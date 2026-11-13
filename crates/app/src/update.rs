//! Update check and in-app update.
//!
//! The check sends a single HTTPS request to the public GitHub releases API of the
//! project. Nothing about the computer, its disks or its user is sent: the request
//! carries only a generic user agent (`ChudClone/<version>`). It goes through the
//! system proxy settings (WinHTTP).
//!
//! An update is applied only after the downloaded package has been checked against the
//! SHA-256 checksum published with the release. A release without a checksum file is
//! never installed automatically: the user is sent to the release page instead.
//!
//! Three installation kinds are handled:
//! - installed (Inno Setup uninstaller next to the executable): the new installer runs
//!   silently, keeps the installation folder and restarts the application;
//! - portable (`portable.flag` next to the executable): the new archive is extracted, the
//!   running executables are renamed to `*.old` and replaced, data stays in place;
//! - anything else (development build): the release page is opened.

use chud_clone_core::{t, tf};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

/// GitHub repository the releases are published in.
pub const REPOSITORY: &str = "BigFatDeuce/Chud-Clone";
/// Upper bound for the API response (the latest release only).
const MAX_API_BYTES: u64 = 4 * 1024 * 1024;
/// Upper bound for a downloaded package, whatever the API claims.
const MAX_PACKAGE_BYTES: u64 = 512 * 1024 * 1024;
/// Upper bound for the checksum file.
const MAX_SUMS_BYTES: u64 = 64 * 1024;

/// How an update can be applied to this copy of the program.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Installer,
    Portable,
    Manual,
}

#[derive(Clone, Debug)]
struct Asset {
    name: String,
    url: String,
    size: u64,
}

/// Result of an update check, as shown by the interface.
#[derive(Serialize, Clone, Debug)]
pub struct UpdateInfo {
    pub current: String,
    pub latest: String,
    pub available: bool,
    /// Release title and notes (Markdown, shown as plain text).
    pub title: String,
    pub notes: String,
    pub published: String,
    pub page_url: String,
    pub method: Method,
    /// Size of the package that would be downloaded.
    pub download_size: Option<u64>,
    #[serde(skip)]
    package: Option<Asset>,
    #[serde(skip)]
    sums: Option<Asset>,
}

impl UpdateInfo {
    /// Release page, only if it belongs to the project's repository.
    pub fn safe_page_url(&self) -> String {
        let base = format!("https://github.com/{REPOSITORY}/releases");
        if self.page_url.starts_with(&(base.clone() + "/")) { self.page_url.clone() } else { base }
    }
}

/// `1.2.3` or `v1.2.3` → (1, 2, 3). Pre-release suffixes are not accepted.
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.trim().trim_start_matches(['v', 'V']).split('.');
    let a = it.next()?.parse().ok()?;
    let b = it.next()?.parse().ok()?;
    let c = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((a, b, c))
}

/// True if `latest` is a strictly newer version than `current`.
fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse_version(latest), parse_version(current)), (Some(l), Some(c)) if l > c)
}

/// Installation kind of the running executable.
pub fn method(portable: bool) -> Method {
    if portable {
        return Method::Portable;
    }
    match exe_dir() {
        Some(d) if d.join("unins000.exe").is_file() => Method::Installer,
        _ => Method::Manual,
    }
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf))
}

/// Interprets the JSON of `GET /repos/{owner}/{repo}/releases/latest`.
fn parse_release(json: &serde_json::Value, current: &str, method: Method) -> Result<UpdateInfo, String> {
    let s = |k: &str| json.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let tag = s("tag_name");
    if parse_version(&tag).is_none() {
        return Err(tf!("réponse inattendue du serveur (version « {tag} »)", "unexpected server response (version \"{tag}\")"));
    }
    let latest = tag.trim_start_matches(['v', 'V']).to_string();
    let assets: Vec<Asset> = json
        .get("assets")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    Some(Asset {
                        name: x.get("name")?.as_str()?.to_string(),
                        url: x.get("browser_download_url")?.as_str()?.to_string(),
                        size: x.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let suffix = match method {
        Method::Installer => Some("-setup.exe"),
        Method::Portable => Some("-portable.zip"),
        Method::Manual => None,
    };
    let package = suffix.and_then(|suf| assets.iter().find(|a| a.name.starts_with("ChudClone-") && a.name.ends_with(suf)).cloned());
    let sums = assets.iter().find(|a| a.name.starts_with("SHA256SUMS")).cloned();
    // Without a checksum file, nothing is installed automatically.
    let method = if package.is_some() && sums.is_some() { method } else { Method::Manual };
    let mut title = s("name");
    if title.is_empty() {
        title = format!("Chud Clone {latest}");
    }
    Ok(UpdateInfo {
        available: is_newer(&latest, current),
        current: current.to_string(),
        latest,
        title,
        notes: s("body"),
        published: s("published_at"),
        page_url: s("html_url"),
        method,
        download_size: package.as_ref().map(|p| p.size),
        package,
        sums,
    })
}

/// Checks the latest published release.
pub fn check(portable: bool) -> Result<UpdateInfo, String> {
    let url = format!("https://api.github.com/repos/{REPOSITORY}/releases/latest");
    let mut body = Vec::new();
    http::get(&url, "application/vnd.github+json", MAX_API_BYTES, &mut |chunk| {
        body.extend_from_slice(chunk);
        Ok(())
    })?;
    let json: serde_json::Value =
        serde_json::from_slice(&body).map_err(|e| tf!("réponse illisible du serveur : {e}", "unreadable server response: {e}"))?;
    parse_release(&json, chud_clone_core::APP_VERSION, method(portable))
}

/// Download progress: (phase, bytes done, bytes expected).
pub type ProgressFn<'a> = &'a dyn Fn(&'static str, u64, u64);

/// Folder that receives the downloaded packages.
fn work_dir() -> PathBuf {
    std::env::temp_dir().join("ChudClone-update")
}

/// Removes what a previous update left behind (downloaded packages, replaced
/// executables of a portable copy). Errors are ignored: a file still in use is simply
/// removed on a later start.
pub fn cleanup(portable: bool) {
    let _ = std::fs::remove_dir_all(work_dir());
    if portable && let Some(dir) = exe_dir() {
        for name in ["chud-clone.exe.old", "chud-clone-cli.exe.old"] {
            let _ = std::fs::remove_file(dir.join(name));
        }
    }
}

/// Expected SHA-256 of `name` in a `SHA256SUMS` file (`<hash>  <name>` lines).
fn expected_hash(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        let hash = it.next()?;
        let file = it.next()?.trim_start_matches('*');
        (file == name && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit())).then(|| hash.to_ascii_lowercase())
    })
}

/// Downloads the package of `info` and checks its SHA-256. Returns its path.
pub fn download(info: &UpdateInfo, progress: ProgressFn) -> Result<PathBuf, String> {
    let (Some(pkg), Some(sums)) = (&info.package, &info.sums) else {
        return Err(t!("aucun paquet installable pour cette version", "no installable package for this version").into());
    };
    if pkg.name.contains(['/', '\\', ':']) || pkg.name.contains("..") {
        return Err(t!("nom de fichier inattendu dans la publication", "unexpected file name in the release").into());
    }
    let dir = work_dir().join(&info.latest);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| tf!("dossier temporaire inaccessible : {e}", "temporary folder unavailable: {e}"))?;

    let mut sums_text = Vec::new();
    http::get(&sums.url, "application/octet-stream", MAX_SUMS_BYTES, &mut |c| {
        sums_text.extend_from_slice(c);
        Ok(())
    })?;
    let expected = expected_hash(&String::from_utf8_lossy(&sums_text), &pkg.name)
        .ok_or_else(|| tf!("somme de contrôle de {} absente de la publication", "checksum of {} missing from the release", pkg.name))?;

    let path = dir.join(&pkg.name);
    let mut file = std::fs::File::create(&path).map_err(|e| tf!("écriture impossible : {e}", "cannot write: {e}"))?;
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    let total = pkg.size;
    progress("download", 0, total);
    let result = http::get(&pkg.url, "application/octet-stream", MAX_PACKAGE_BYTES, &mut |c| {
        hasher.update(c);
        file.write_all(c).map_err(|e| tf!("écriture impossible : {e}", "cannot write: {e}"))?;
        done += c.len() as u64;
        progress("download", done, total);
        Ok(())
    });
    drop(file);
    if let Err(e) = result {
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    progress("verify", done, total);
    let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if got != expected || (total > 0 && done != total) {
        let _ = std::fs::remove_file(&path);
        return Err(t!(
            "le fichier téléchargé ne correspond pas à la somme de contrôle publiée : il a été supprimé",
            "the downloaded file does not match the published checksum: it has been deleted"
        )
        .into());
    }
    Ok(path)
}

/// Starts the downloaded installer: silent installation in the same folder, then
/// restart of the application. The caller exits right after.
#[cfg(windows)]
pub fn run_installer(setup: &Path) -> Result<(), String> {
    shell_open(setup, Some("/SILENT /SUPPRESSMSGBOXES /NORESTART /relaunch=1"))
}

/// Replaces the executables of a portable copy with those of the downloaded archive.
/// Returns the path of the new main executable.
#[cfg(windows)]
pub fn apply_portable(zip: &Path) -> Result<PathBuf, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let target = exe_dir().ok_or_else(|| t!("dossier du programme introuvable", "program folder not found").to_string())?;
    let extract = zip.with_extension("extract");
    let _ = std::fs::remove_dir_all(&extract);
    std::fs::create_dir_all(&extract).map_err(|e| tf!("dossier temporaire inaccessible : {e}", "temporary folder unavailable: {e}"))?;
    // tar.exe ships with Windows 10 (1803) and later and reads ZIP archives.
    let tar = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows")).join(r"System32\tar.exe");
    let status = std::process::Command::new(tar)
        .arg("-xf")
        .arg(zip)
        .arg("-C")
        .arg(&extract)
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|e| tf!("extraction impossible : {e}", "cannot extract: {e}"))?;
    if !status.success() {
        return Err(t!("extraction de l'archive impossible", "cannot extract the archive").into());
    }
    let source = [extract.clone()]
        .into_iter()
        .chain(std::fs::read_dir(&extract).into_iter().flatten().flatten().map(|e| e.path()))
        .find(|d| d.join("chud-clone.exe").is_file())
        .ok_or_else(|| t!("archive inattendue : programme absent", "unexpected archive: program missing").to_string())?;

    // A running executable cannot be overwritten but it can be renamed.
    let exes = ["chud-clone.exe", "chud-clone-cli.exe"];
    let mut renamed = Vec::new();
    for name in exes {
        let cur = target.join(name);
        if !cur.exists() {
            continue;
        }
        let old = target.join(format!("{name}.old"));
        let _ = std::fs::remove_file(&old);
        if let Err(e) = std::fs::rename(&cur, &old) {
            for (c, o) in renamed.iter().rev() {
                let _ = std::fs::rename(o, c);
            }
            return Err(tf!(
                "impossible de remplacer {name} : {e}. Le dossier du programme est peut-être protégé en écriture.",
                "cannot replace {name}: {e}. The program folder may be write-protected."
            ));
        }
        renamed.push((cur, old));
    }
    let copy_all = || -> std::io::Result<()> {
        for name in exes.iter().chain(["README.txt"].iter()) {
            let from = source.join(name);
            if from.is_file() {
                std::fs::copy(&from, target.join(name))?;
            }
        }
        let lic = source.join("licenses");
        if lic.is_dir() {
            std::fs::create_dir_all(target.join("licenses"))?;
            for e in std::fs::read_dir(&lic)?.flatten() {
                if e.path().is_file() {
                    std::fs::copy(e.path(), target.join("licenses").join(e.file_name()))?;
                }
            }
        }
        Ok(())
    };
    if let Err(e) = copy_all() {
        // Puts the previous version back.
        for (c, o) in renamed.iter().rev() {
            let _ = std::fs::remove_file(c);
            let _ = std::fs::rename(o, c);
        }
        return Err(tf!("copie de la nouvelle version impossible : {e}", "cannot copy the new version: {e}"));
    }
    let _ = std::fs::remove_dir_all(&extract);
    Ok(target.join("chud-clone.exe"))
}

/// Opens a file or an address with the associated program (`ShellExecuteW`).
#[cfg(windows)]
pub fn shell_open(target: impl AsRef<std::ffi::OsStr>, params: Option<&str>) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let wide = |s: &std::ffi::OsStr| s.encode_wide().chain(Some(0)).collect::<Vec<u16>>();
    let file = wide(target.as_ref());
    let verb = wide("open".as_ref());
    let params = params.map(|p| wide(p.as_ref()));
    // SAFETY: zero-terminated UTF-16 strings that outlive the call; null pointers are allowed.
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ref().map_or(std::ptr::null(), |p| p.as_ptr()),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    } as isize;
    if r > 32 {
        Ok(())
    } else if r == 5 {
        Err(t!("lancement refusé (autorisation administrateur annulée)", "launch refused (administrator permission cancelled)").into())
    } else {
        Err(tf!("lancement impossible (code {r})", "cannot launch (code {r})"))
    }
}

/// HTTPS GET through WinHTTP (system TLS, system proxy settings).
#[cfg(windows)]
mod http {
    use chud_clone_core::{t, tf};
    use core::ffi::c_void;
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Networking::WinHttp::*;

    struct Handle(*mut c_void);
    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: handle returned by WinHTTP, closed once.
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn last_error() -> String {
        // SAFETY: no precondition.
        let code = unsafe { GetLastError() };
        match code {
            12002 => t!("délai de connexion dépassé", "connection timed out").into(),
            12007 | 12029 => t!("aucune connexion à Internet ou serveur injoignable", "no Internet connection or server unreachable").into(),
            12175 | 12045 | 12044 => t!("connexion sécurisée refusée (certificat)", "secure connection refused (certificate)").into(),
            _ => tf!("erreur réseau (code {code})", "network error (code {code})"),
        }
    }

    fn check(h: *mut c_void) -> Result<Handle, String> {
        if h.is_null() { Err(last_error()) } else { Ok(Handle(h)) }
    }

    /// Downloads `url` (HTTPS only) and passes the data to `sink` in chunks.
    pub fn get(url: &str, accept: &str, max: u64, sink: &mut dyn FnMut(&[u8]) -> Result<(), String>) -> Result<(), String> {
        let rest = url.strip_prefix("https://").ok_or_else(|| t!("adresse non sécurisée refusée", "insecure address refused").to_string())?;
        let (host, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let agent = wide(&format!("ChudClone/{}", chud_clone_core::APP_VERSION));
        let host_w = wide(host);
        let path_w = wide(path);
        let verb = wide("GET");
        let headers: Vec<u16> = format!("Accept: {accept}\r\n").encode_utf16().collect();
        // SAFETY: every pointer is a zero-terminated UTF-16 buffer living until the end of
        // the function; handles are closed by `Handle` in reverse order of creation.
        unsafe {
            let session = check(WinHttpOpen(agent.as_ptr(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, std::ptr::null(), std::ptr::null(), 0))?;
            WinHttpSetTimeouts(session.0, 15_000, 15_000, 30_000, 60_000);
            let conn = check(WinHttpConnect(session.0, host_w.as_ptr(), INTERNET_DEFAULT_HTTPS_PORT, 0))?;
            let req = check(WinHttpOpenRequest(
                conn.0,
                verb.as_ptr(),
                path_w.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                WINHTTP_FLAG_SECURE,
            ))?;
            if WinHttpSendRequest(req.0, headers.as_ptr(), headers.len() as u32, std::ptr::null(), 0, 0, 0) == 0
                || WinHttpReceiveResponse(req.0, std::ptr::null_mut()) == 0
            {
                return Err(last_error());
            }
            let mut status = 0u32;
            let mut len = 4u32;
            if WinHttpQueryHeaders(
                req.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                std::ptr::null(),
                (&mut status as *mut u32).cast(),
                &mut len,
                std::ptr::null_mut(),
            ) == 0
            {
                return Err(last_error());
            }
            match status {
                200 => {}
                404 => return Err(t!("aucune version publiée trouvée", "no published release found").into()),
                403 | 429 => {
                    return Err(
                        t!("le serveur limite les requêtes : réessayez plus tard", "the server is rate-limiting requests: try again later").into()
                    );
                }
                s => return Err(tf!("réponse du serveur : HTTP {s}", "server response: HTTP {s}")),
            }
            let mut buf = vec![0u8; 64 * 1024];
            let mut total = 0u64;
            loop {
                let mut read = 0u32;
                if WinHttpReadData(req.0, buf.as_mut_ptr().cast(), buf.len() as u32, &mut read) == 0 {
                    return Err(last_error());
                }
                if read == 0 {
                    break;
                }
                total += read as u64;
                if total > max {
                    return Err(
                        t!("réponse plus grande que prévu : téléchargement interrompu", "response larger than expected: download stopped").into()
                    );
                }
                sink(&buf[..read as usize])?;
            }
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod http {
    pub fn get(_: &str, _: &str, _: u64, _: &mut dyn FnMut(&[u8]) -> Result<(), String>) -> Result<(), String> {
        Err(chud_clone_core::t!("non pris en charge sur cette plateforme", "not supported on this platform").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.10.0"), Some((0, 10, 0)));
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3-beta"), None);
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }

    fn release() -> serde_json::Value {
        serde_json::json!({
            "tag_name": "v9.1.0",
            "name": "Chud Clone 9.1.0",
            "body": "Notes",
            "published_at": "2026-11-13T12:00:00Z",
            "html_url": format!("https://github.com/{REPOSITORY}/releases/tag/v9.1.0"),
            "assets": [
                { "name": "ChudClone-9.1.0-setup.exe", "browser_download_url": "https://example.invalid/s.exe", "size": 100 },
                { "name": "ChudClone-9.1.0-portable.zip", "browser_download_url": "https://example.invalid/p.zip", "size": 200 },
                { "name": "SHA256SUMS-9.1.0.txt", "browser_download_url": "https://example.invalid/sums", "size": 10 }
            ]
        })
    }

    #[test]
    fn release_parsing() {
        let i = parse_release(&release(), "0.2.0", Method::Installer).unwrap();
        assert!(i.available);
        assert_eq!(i.latest, "9.1.0");
        assert_eq!(i.method, Method::Installer);
        assert_eq!(i.download_size, Some(100));
        let p = parse_release(&release(), "0.2.0", Method::Portable).unwrap();
        assert_eq!(p.download_size, Some(200));
        assert!(!parse_release(&release(), "9.1.0", Method::Portable).unwrap().available);
        assert!(i.safe_page_url().ends_with("/releases/tag/v9.1.0"));
    }

    #[test]
    fn no_checksum_means_manual() {
        let mut r = release();
        r["assets"].as_array_mut().unwrap().retain(|a| !a["name"].as_str().unwrap().starts_with("SHA256SUMS"));
        assert_eq!(parse_release(&r, "0.2.0", Method::Installer).unwrap().method, Method::Manual);
    }

    #[test]
    fn foreign_page_url_is_replaced() {
        let mut r = release();
        r["html_url"] = "https://example.invalid/phishing".into();
        let i = parse_release(&r, "0.2.0", Method::Manual).unwrap();
        assert_eq!(i.safe_page_url(), format!("https://github.com/{REPOSITORY}/releases"));
    }

    /// Real request to GitHub (network): `cargo test -p chud-clone-app -- --ignored`.
    #[test]
    #[ignore]
    fn live_check() {
        let i = check(false).expect("update check");
        println!("{i:?}");
        assert!(parse_version(&i.latest).is_some());
        assert!(i.page_url.starts_with(&format!("https://github.com/{REPOSITORY}/releases/")));
    }

    #[test]
    fn checksum_lookup() {
        let h = "a".repeat(64);
        let sums = format!("{h}  ChudClone-9.1.0-setup.exe\n{}  other.zip\n", "b".repeat(64));
        assert_eq!(expected_hash(&sums, "ChudClone-9.1.0-setup.exe"), Some(h));
        assert_eq!(expected_hash(&sums, "missing.exe"), None);
        assert_eq!(expected_hash("xyz  ChudClone-9.1.0-setup.exe", "ChudClone-9.1.0-setup.exe"), None);
    }
}
