//! Bridge the user's chosen UI language to the standalone "Hey Vortex" voice
//! assistant (a separate Python process). The voice listener reads
//! `~/.local/share/vortex/voice/lang` at startup and watches it for changes, so
//! writing the locale here makes the assistant follow the app's language.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

fn bridge_path() -> Option<PathBuf> {
    // Linux keeps its exact path: this file is a BRIDGE, read by the voice
    // assistant scripts outside this process, so moving it would break a
    // consumer that knows nothing about the seam. Same reasoning as
    // `file_consent::flag_path`.
    #[cfg(target_os = "linux")]
    {
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join(".local/share/vortex/voice/lang"))
    }
    // Everywhere else, through the seam. `$HOME` is unset on Windows, so this
    // resolved to `None` and the language could be neither read nor written.
    #[cfg(not(target_os = "linux"))]
    {
        Some(
            vortex_l3_daemon::core::platform::paths()
                .config()?
                .join("voice")
                .join("lang"),
        )
    }
}

/// Persist the active language (en/ru/uz) for the voice assistant. Written
/// atomically (temp + rename) so the watcher never reads a half-written file.
#[tauri::command]
pub(crate) fn set_voice_lang(code: String) -> Result<(), String> {
    let code = code.trim().to_lowercase();
    if !matches!(code.as_str(), "en" | "ru" | "uz") {
        return Err(format!("unsupported voice language: {code}"));
    }
    let path = bridge_path().ok_or("no HOME")?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp).map_err(|e| e.to_string())?;
        f.write_all(code.as_bytes()).map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(())
}
